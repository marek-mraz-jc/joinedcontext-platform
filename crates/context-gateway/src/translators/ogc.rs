//! NGSI-LD as OGC API - Features Part 1 Core (T-0159, EP-29, EP-30, EP-31, EP-32, EP-39).
//!
//! A GIS client speaks collections and features, not entities and attributes, and it will not
//! learn. So the whole representation is a rewriting of names in both directions: a collection
//! is an entity type, a feature id is an entity URN, `bbox` and `datetime` are an NGSI-LD
//! `geoQ` and `temporalQ`. Both directions are pure functions here; the routing and the one
//! broker call live in the handler, which is what makes the rewriting testable on its own.
//!
//! The query parameters are translated into the same NGSI-LD parameter names the canonical
//! surface uses, and then handed to the same policy path. A GIS client therefore cannot reach
//! data the NGSI-LD surface would refuse, because after the translation there is only one
//! surface left (EP-06, EP-07).

use crate::translators::geojson;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The media type of the landing page, the conformance list and the collection documents.
pub const JSON: &str = "application/json";

/// The media type of `items`, which is the GeoJSON one.
pub const GEOJSON: &str = geojson::MEDIA_TYPE;

/// The only coordinate reference system NGSI-LD stores, and so the only one served (EP-32).
pub const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";

/// The conformance classes this representation actually satisfies (EP-30).
///
/// Claiming a class is a promise to a client that will act on it: QGIS offers a filter box
/// when a service claims CQL2 and shows the user an error when the service then refuses the
/// filter. So the list is what the code does and nothing more. The five are the five EP-30
/// names, and each is implemented here: `core` and `geojson` by the collection and item
/// documents, `oas30` by [`api_document`], `crs` by the one system NGSI-LD stores, and
/// `basic-cql2` by [`crate::translators::cql2`], which compiles the whole subset EP-35 lists
/// and refuses everything else by name.
pub const CONFORMANCE: [&str; 5] = [
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/core",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/oas30",
    "http://www.opengis.net/spec/ogcapi-features-1/1.0/conf/geojson",
    "http://www.opengis.net/spec/ogcapi-features-2/1.0/conf/crs",
    "http://www.opengis.net/spec/ogcapi-features-3/1.0/conf/basic-cql2",
];

/// The media type of the API document, which OGC pins to the version it is written in.
pub const OPENAPI: &str = "application/vnd.oai.openapi+json;version=3.0";

/// The default and the largest page an items request may ask for (EP-36).
pub const DEFAULT_LIMIT: usize = 10;

/// A query parameter the caller sent that this representation cannot honour.
///
/// Named rather than generic, because a client that gets `400` without knowing which
/// parameter was wrong retries the same request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{parameter}: {detail}")]
pub struct ParamError {
    /// The parameter as the caller spelled it.
    pub parameter: &'static str,
    /// What is wrong with it, in the words a client shows its user.
    pub detail: String,
}

impl ParamError {
    fn new(parameter: &'static str, detail: impl Into<String>) -> Self {
        Self {
            parameter,
            detail: detail.into(),
        }
    }
}

/// The OGC root of one endpoint.
fn root(endpoint: &str) -> String {
    format!("{endpoint}/ogc/features")
}

/// One link object, which is the only thing OGC uses to join its resources.
fn link(rel: &str, media_type: &str, href: String, title: &str) -> Value {
    json!({ "rel": rel, "type": media_type, "href": href, "title": title })
}

/// The landing page: what this service is and the three doors out of it (EP-29).
pub fn landing(endpoint: &str, title: &str, description: &str) -> Value {
    let root = root(endpoint);
    json!({
        "title": title,
        "description": description,
        "links": [
            link("self", JSON, format!("{root}/"), "This document"),
            link("service-desc", OPENAPI, format!("{root}/api"), "The API description"),
            link("conformance", JSON, format!("{root}/conformance"), "Conformance classes"),
            link("data", JSON, format!("{root}/collections"), "Collections"),
            link("alternate", "application/ld+json", format!("{endpoint}/ngsi-ld/v1/entities"),
                 "The same data as NGSI-LD"),
        ],
    })
}

/// The API description of one endpoint, as OpenAPI 3.0.3 (EP-40).
///
/// Generated rather than written: an endpoint's collections are the types its grants leave
/// visible, so two callers of the same service legitimately get two documents, and a static
/// file would describe collections the reader may not have. The paths are the paths the router
/// actually serves — anything a client finds here is reachable, and anything reachable is here,
/// which is what the `oas30` class of EP-30 promises.
///
/// The document describes the read surface only, because the representation has no write half
/// (EP-39): no `requestBody` anywhere in it, and the operations a client may try are exactly
/// `get`.
pub fn api_document(endpoint: &str, title: &str, description: &str, types: &[String]) -> Value {
    let root = root(endpoint);
    let response = |media_type: &str, what: &str| {
        json!({
            "200": {
                "description": what,
                "content": { media_type: { "schema": { "type": "object" } } },
            },
            "400": { "$ref": "#/components/responses/BadRequest" },
            "404": { "$ref": "#/components/responses/NotFound" },
        })
    };
    let operation = |id: &str, summary: &str, media_type: &str, parameters: Value| {
        json!({
            "get": {
                "operationId": id,
                "summary": summary,
                "tags": ["Features"],
                "parameters": parameters,
                "responses": response(media_type, summary),
            }
        })
    };
    let parameter = |name: &str, place: &str, what: &str, schema: Value| {
        json!({
            "name": name,
            "in": place,
            "description": what,
            "required": place == "path",
            "schema": schema,
            "style": if place == "path" { "simple" } else { "form" },
            "explode": false,
        })
    };
    let collection_id = parameter(
        "collectionId",
        "path",
        "The entity type this collection serves",
        json!({ "type": "string", "enum": types }),
    );
    let feature_id = parameter(
        "featureId",
        "path",
        "The NGSI-LD URN of one entity",
        json!({ "type": "string" }),
    );

    json!({
        "openapi": "3.0.3",
        "info": {
            "title": title,
            "description": description,
            "version": "1.0.0",
        },
        "servers": [{ "url": root, "description": "This endpoint" }],
        "tags": [{ "name": "Features", "description": "OGC API - Features Part 1" }],
        "paths": {
            "/": operation("getLandingPage", "The landing page", JSON, json!([])),
            "/api": operation("getApiDescription", "This document", OPENAPI, json!([])),
            "/conformance": operation(
                "getConformanceClasses", "The conformance classes claimed", JSON, json!([])),
            "/collections": operation(
                "getCollections", "The collections this endpoint serves", JSON, json!([])),
            "/collections/{collectionId}": json!({
                "parameters": [collection_id],
                "get": operation("getCollection", "One collection", JSON, json!([]))["get"],
            }),
            "/collections/{collectionId}/items": json!({
                "parameters": [collection_id],
                "get": operation("getFeatures", "One page of features", GEOJSON, json!([
                    parameter("bbox", "query", "A bounding box in CRS84, as four or six numbers",
                              json!({ "type": "array", "minItems": 4, "maxItems": 6,
                                      "items": { "type": "number" } })),
                    parameter("datetime", "query",
                              "An RFC 3339 instant or interval, open at either end",
                              json!({ "type": "string" })),
                    parameter("filter", "query",
                              "A CQL2 expression over the collection's properties",
                              json!({ "type": "string" })),
                    parameter("filter-lang", "query", "The language `filter` is written in",
                              json!({ "type": "string",
                                      "enum": [crate::translators::cql2::LANG],
                                      "default": crate::translators::cql2::LANG })),
                    parameter("crs", "query", "The coordinate reference system of the answer",
                              json!({ "type": "string", "enum": [CRS84], "default": CRS84 })),
                    parameter("limit", "query", "How many features one page carries",
                              json!({ "type": "integer", "minimum": 1,
                                      "default": DEFAULT_LIMIT })),
                    parameter("next", "query", "The opaque cursor of the `next` link",
                              json!({ "type": "string" })),
                ]))["get"],
            }),
            "/collections/{collectionId}/items/{featureId}": json!({
                "parameters": [collection_id, feature_id],
                "get": operation("getFeature", "One feature", GEOJSON, json!([]))["get"],
            }),
        },
        "components": {
            "responses": {
                "BadRequest": {
                    "description": "A parameter this endpoint cannot honour, named in the body",
                    "content": { "application/problem+json": {
                        "schema": { "type": "object" } } },
                },
                "NotFound": {
                    "description": "No such collection or feature, or none this caller may read",
                    "content": { "application/problem+json": {
                        "schema": { "type": "object" } } },
                },
            },
        },
    })
}

/// The classes claimed, which is exactly [`CONFORMANCE`] (EP-30).
pub fn conformance() -> Value {
    json!({ "conformsTo": CONFORMANCE })
}

/// The collection list: one entry per entity type the caller may see (EP-31).
pub fn collections(endpoint: &str, types: &[String], description: &str) -> Value {
    let root = root(endpoint);
    let entries: Vec<Value> = types
        .iter()
        .map(|name| collection(endpoint, name, description, None))
        .collect();
    json!({
        "links": [
            link("self", JSON, format!("{root}/collections"), "Collections"),
        ],
        "collections": entries,
    })
}

/// One collection: an entity type, its extent and the one CRS the platform stores (EP-32).
///
/// `extent` is what the caller's own projected data spans, computed by the handler from a
/// bounded page; `None` advertises the unbounded extent, which is what OGC uses for "not
/// known" and is the honest answer for an empty or unsampled collection.
pub fn collection(endpoint: &str, name: &str, description: &str, extent: Option<&Extent>) -> Value {
    let root = root(endpoint);
    let bbox = extent
        .and_then(|extent| extent.bbox)
        .map_or(json!([[-180.0, -90.0, 180.0, 90.0]]), |bbox| {
            json!([[bbox[0], bbox[1], bbox[2], bbox[3]]])
        });
    let interval = extent.map_or(json!([[Value::Null, Value::Null]]), |extent| {
        json!([[
            extent.start.clone().map_or(Value::Null, Value::String),
            extent.end.clone().map_or(Value::Null, Value::String),
        ]])
    });
    json!({
        "id": name,
        "title": name,
        "description": description,
        "itemType": "feature",
        "crs": [CRS84],
        "storageCrs": CRS84,
        "extent": {
            "spatial": { "bbox": bbox, "crs": CRS84 },
            "temporal": { "interval": interval, "trs": "http://www.opengis.net/def/uom/ISO-8601/0/Gregorian" },
        },
        "links": [
            link("self", JSON, format!("{root}/collections/{name}"), name),
            link("items", GEOJSON, format!("{root}/collections/{name}/items"), name),
        ],
    })
}

/// What a collection's data spans, as far as the caller may see it (EP-32).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extent {
    /// `[minx, miny, maxx, maxy]` in CRS84, absent when nothing carried a geometry.
    pub bbox: Option<[f64; 4]>,
    /// The earliest `observedAt` seen, as written.
    pub start: Option<String>,
    /// The latest `observedAt` seen, as written.
    pub end: Option<String>,
}

/// The extent of a page of entities: the geometries' bounding box and the `observedAt` span.
///
/// Only the entities the caller may read are sampled, so two callers with different grants
/// legitimately see two different extents. That is the same rule the data path follows and not
/// a leak: an extent computed over data the caller cannot fetch would advertise its existence.
pub fn extent_of(entities: &Value) -> Extent {
    let mut extent = Extent::default();
    for entity in entities.as_array().into_iter().flatten() {
        if let Some(feature) = geojson::feature(entity) {
            for [x, y] in positions(&feature["geometry"]) {
                extent.bbox = Some(match extent.bbox {
                    None => [x, y, x, y],
                    Some([minx, miny, maxx, maxy]) => {
                        [minx.min(x), miny.min(y), maxx.max(x), maxy.max(y)]
                    }
                });
            }
        }
        for observed in observed_times(entity) {
            if extent.start.as_deref().is_none_or(|start| observed < start) {
                extent.start = Some(observed.to_owned());
            }
            if extent.end.as_deref().is_none_or(|end| observed > end) {
                extent.end = Some(observed.to_owned());
            }
        }
    }
    extent
}

/// Every `[x, y]` inside a GeoJSON geometry, whatever its nesting depth.
fn positions(geometry: &Value) -> Vec<[f64; 2]> {
    fn walk(value: &Value, out: &mut Vec<[f64; 2]>) {
        match value {
            Value::Array(items) => {
                // A position is an array whose first two members are numbers; anything else
                // is a level of nesting on the way to one.
                if let (Some(x), Some(y)) = (
                    items.first().and_then(Value::as_f64),
                    items.get(1).and_then(Value::as_f64),
                ) {
                    out.push([x, y]);
                } else {
                    for item in items {
                        walk(item, out);
                    }
                }
            }
            Value::Object(members) => {
                for key in ["coordinates", "geometries"] {
                    if let Some(nested) = members.get(key) {
                        walk(nested, out);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(geometry, &mut out);
    out
}

/// Every `observedAt` an entity carries, on itself or on one of its attributes.
fn observed_times(entity: &Value) -> Vec<&str> {
    let Some(members) = entity.as_object() else {
        return Vec::new();
    };
    let mut times: Vec<&str> = members
        .get("observedAt")
        .and_then(Value::as_str)
        .into_iter()
        .collect();
    for value in members.values() {
        if let Some(observed) = value.get("observedAt").and_then(Value::as_str) {
            times.push(observed);
        }
    }
    times
}

/// One page of features (EP-33, EP-36, EP-38).
///
/// `raw_query` is the caller's query string exactly as it arrived, so the `self` link is the
/// request and the `next` link is the request with one parameter replaced. Entities without a
/// geometry are dropped rather than refused: a page that happens to hold one is still a page.
pub fn items(
    endpoint: &str,
    name: &str,
    entities: &Value,
    limit: usize,
    offset: usize,
    raw_query: &str,
    timestamp: &str,
) -> Value {
    let root = root(endpoint);
    let features: Vec<Value> = entities
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entity| feature(endpoint, name, entity))
        .collect();
    let returned = features.len();

    let items_url = format!("{root}/collections/{name}/items");
    let mut links = vec![
        link("self", GEOJSON, with_query(&items_url, raw_query), name),
        link(
            "collection",
            JSON,
            format!("{root}/collections/{name}"),
            name,
        ),
    ];
    // A full page means there may be another one. Asking the broker for the count of what the
    // caller may see costs a second query on every page, so the link is offered on the only
    // evidence a page carries and the last one is simply empty (EP-36).
    if entities.as_array().is_some_and(|list| list.len() >= limit) {
        let next = replace_cursor(raw_query, &cursor(offset + limit));
        links.push(link(
            "next",
            GEOJSON,
            with_query(&items_url, &next),
            "Next page",
        ));
    }

    json!({
        "type": "FeatureCollection",
        "numberReturned": returned,
        "timeStamp": timestamp,
        "features": features,
        "links": links,
    })
}

/// One feature, with the links back to what it really is (EP-33, EP-38).
pub fn feature(endpoint: &str, name: &str, entity: &Value) -> Option<Value> {
    let mut feature = geojson::feature(entity)?;
    let Some(id) = feature.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return Some(feature);
    };
    let links = vec![
        link(
            "self",
            GEOJSON,
            format!("{}/collections/{name}/items/{id}", root(endpoint)),
            &id,
        ),
        link(
            "alternate",
            "application/ld+json",
            format!("{endpoint}/ngsi-ld/v1/entities/{id}"),
            "The canonical NGSI-LD entity",
        ),
    ];
    if let Value::Object(members) = &mut feature {
        members.insert("links".to_owned(), Value::Array(links));
    }
    Some(feature)
}

/// The opaque page cursor: the NGSI-LD offset, encoded so no client is tempted to compute one
/// (EP-36).
pub fn cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(format!("offset={offset}"))
}

/// The offset behind a cursor, or nothing when it was not one this gateway wrote.
///
/// A cursor is caller-supplied input like any other: it is decoded, checked and parsed, never
/// trusted, and a cursor that does not decode starts the collection from the beginning rather
/// than erroring, because a stale bookmark is not a client fault.
pub fn offset_of(raw: &str) -> Option<usize> {
    let decoded = URL_SAFE_NO_PAD.decode(raw).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    text.strip_prefix("offset=")?.parse().ok()
}

/// `bbox` as an NGSI-LD `geoQ`, which is three parameters (EP-34).
///
/// The 6-value form carries a minimum and maximum elevation which NGSI-LD's 2D `geoQ` cannot
/// express; the horizontal box is used and the elevation dropped, which can only widen the
/// answer the caller filters locally, never narrow it past what they asked for.
pub fn bbox(raw: &str) -> Result<Vec<(String, String)>, ParamError> {
    let numbers: Result<Vec<f64>, _> = raw
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect();
    let numbers = numbers.map_err(|_| ParamError::new("bbox", "every value must be a number"))?;
    let (minx, miny, maxx, maxy) = match numbers.len() {
        4 => (numbers[0], numbers[1], numbers[2], numbers[3]),
        6 => (numbers[0], numbers[1], numbers[3], numbers[4]),
        _ => {
            return Err(ParamError::new(
                "bbox",
                "expected 4 values (minx,miny,maxx,maxy) or 6 with elevation",
            ))
        }
    };
    if minx > maxx || miny > maxy {
        return Err(ParamError::new(
            "bbox",
            "the lower corner must not be greater than the upper corner",
        ));
    }
    let ring = json!([[
        [minx, miny],
        [maxx, miny],
        [maxx, maxy],
        [minx, maxy],
        [minx, miny]
    ]]);
    Ok(vec![
        ("georel".to_owned(), "intersects".to_owned()),
        ("geometry".to_owned(), "Polygon".to_owned()),
        ("coordinates".to_owned(), ring.to_string()),
    ])
}

/// `datetime` as an NGSI-LD `temporalQ`, which is up to three parameters (EP-34).
///
/// An instant becomes a closed interval of itself rather than an equality, because NGSI-LD has
/// no `at` relation and a client asking for one moment means the observations of that moment.
pub fn datetime(raw: &str) -> Result<Vec<(String, String)>, ParamError> {
    let pair = |rel: &str, at: &str| -> Vec<(String, String)> {
        vec![
            ("timerel".to_owned(), rel.to_owned()),
            ("timeAt".to_owned(), at.to_owned()),
        ]
    };
    let instant = |value: &str| -> Result<String, ParamError> {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|_| value.to_owned())
            .map_err(|_| ParamError::new("datetime", format!("{value} is not an RFC 3339 instant")))
    };

    match raw.split_once('/') {
        None => {
            let at = instant(raw)?;
            let mut params = pair("between", &at);
            params.push(("endTimeAt".to_owned(), at));
            Ok(params)
        }
        Some(("..", "..")) | Some(("", "")) => Err(ParamError::new(
            "datetime",
            "an interval open at both ends selects everything; omit the parameter instead",
        )),
        Some((start, "..")) | Some((start, "")) => Ok(pair("after", &instant(start)?)),
        Some(("..", end)) | Some(("", end)) => Ok(pair("before", &instant(end)?)),
        Some((start, end)) => {
            let (start, end) = (instant(start)?, instant(end)?);
            if start > end {
                return Err(ParamError::new(
                    "datetime",
                    "the interval ends before it starts",
                ));
            }
            let mut params = pair("between", &start);
            params.push(("endTimeAt".to_owned(), end));
            Ok(params)
        }
    }
}

/// The `crs` parameter: CRS84 is the only system the platform stores, so anything else is a
/// coordinate transformation the gateway would have to invent (EP-32).
pub fn crs(raw: &str) -> Result<(), ParamError> {
    if raw == CRS84 || raw == "http://www.opengis.net/def/crs/OGC/1.3/CRS84h" {
        return Ok(());
    }
    Err(ParamError::new(
        "crs",
        format!("this endpoint stores {CRS84} only"),
    ))
}

/// The text of a language map for the caller's `Accept-Language`, or the best thing there is.
///
/// Not a full RFC 4647 match: the first tag is compared whole and then by its primary subtag,
/// which is what a two-locale municipal deployment needs. A caller who asks for nothing, or for
/// a language nobody wrote, gets the declared default and then whatever exists, because an
/// empty title is worse than a title in the wrong language.
pub fn localized<'a>(
    texts: &'a BTreeMap<String, String>,
    accept_language: Option<&str>,
    default_locale: Option<&str>,
) -> &'a str {
    let wanted = accept_language
        .and_then(|header| header.split(',').next())
        .map(|tag| tag.split(';').next().unwrap_or(tag).trim().to_owned());
    let candidates = [wanted.as_deref(), default_locale];
    for candidate in candidates.into_iter().flatten() {
        if let Some(text) = texts.get(candidate) {
            return text;
        }
        let primary = candidate.split('-').next().unwrap_or(candidate);
        if let Some((_, text)) = texts
            .iter()
            .find(|(locale, _)| locale.split('-').next() == Some(primary))
        {
            return text;
        }
    }
    texts.values().next().map_or("", String::as_str)
}

/// A URL with a query string appended, or without the `?` when there is none.
fn with_query(url: &str, query: &str) -> String {
    if query.is_empty() {
        url.to_owned()
    } else {
        format!("{url}?{query}")
    }
}

/// The caller's query string with the page cursor replaced by a new one.
///
/// Textual because the rest of the query is the caller's and must survive byte for byte: a
/// `next` link that re-encoded a filter would change the query it is supposed to continue.
fn replace_cursor(raw_query: &str, cursor: &str) -> String {
    let mut kept: Vec<&str> = raw_query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("next=") && *pair != "next")
        .collect();
    let next = format!("next={cursor}");
    kept.push(&next);
    kept.join("&")
}
