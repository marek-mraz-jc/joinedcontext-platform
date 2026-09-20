//! Edge cases of `translators::ogc::api_document` (T-1949, EP-30, EP-39, EP-40, EP-26, MP-02).
//!
//! Contract, in one sentence: the OpenAPI document describes exactly the read surface the router
//! serves, for exactly the entity types this caller's grants leave visible, and nothing in it is a
//! write — everything a client finds here is reachable, and anything reachable is here.
//!
//! It is generated per caller for one reason: a static file would name collections the reader may
//! not have, and a type name is data (EP-31). So the two things worth attacking are the type list
//! — a name outside it must appear nowhere — and the operations, where one `requestBody` would
//! advertise a write half this representation does not have (EP-39).

use context_gateway::translators::ogc::api_document;
use serde_json::{json, Value};

const ENDPOINT: &str = "https://city.example/api/endpoint/mluyob4nz52lok3ssk7pgn5vwt";

fn document(types: &[&str]) -> Value {
    api_document(
        ENDPOINT,
        "Ovzdušie",
        "The air quality of the city",
        &types
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
}

fn paths(document: &Value) -> &serde_json::Map<String, Value> {
    document["paths"].as_object().expect("paths")
}

/// EP-39: the representation has no write half, so no operation but `get` and no `requestBody`
/// anywhere. A client that finds a `post` here tries it.
#[test]
fn nothing_in_the_document_is_a_write() {
    let document = document(&["AirQualityObserved"]);

    for (path, item) in paths(&document) {
        let members = item.as_object().expect("a path item");
        for key in members.keys() {
            assert!(key == "get" || key == "parameters", "{path} offers {key}");
        }
    }
    let rendered = serde_json::to_string(&document).expect("serialises");
    for forbidden in [
        "requestBody",
        "\"post\"",
        "\"put\"",
        "\"patch\"",
        "\"delete\"",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "{forbidden} is in the document"
        );
    }
}

/// EP-31: the collections are the caller's own types, and a type this caller may not see appears
/// nowhere in the document — not in the enum, not in an example, not in a description.
#[test]
fn a_type_outside_the_callers_grants_appears_nowhere() {
    let document = document(&["AirQualityObserved"]);
    let rendered = serde_json::to_string(&document).expect("serialises");

    assert!(rendered.contains("AirQualityObserved"));
    for hidden in ["Depot", "Vehicle", "OperatorContact"] {
        assert!(
            !rendered.contains(hidden),
            "{hidden} leaked into the document"
        );
    }
}

/// The enum is exactly the list, in the order it was given, and a caller with no types gets an
/// empty enum rather than a missing one — which would read as "any collection".
#[test]
fn the_collection_enum_is_exactly_the_types_it_was_given() {
    let both = document(&["Depot", "AirQualityObserved"]);
    let enumerated =
        &paths(&both)["/collections/{collectionId}"]["parameters"][0]["schema"]["enum"];
    assert_eq!(enumerated, &json!(["Depot", "AirQualityObserved"]));

    let empty = document(&[]);
    assert_eq!(
        &paths(&empty)["/collections/{collectionId}"]["parameters"][0]["schema"]["enum"],
        &json!([])
    );
}

/// The paths are the paths the router serves: every one of them and no other.
#[test]
fn the_paths_are_the_ones_the_router_serves() {
    let document = document(&["Depot"]);
    let mut served: Vec<&String> = paths(&document).keys().collect();
    served.sort();
    assert_eq!(
        served,
        vec![
            "/",
            "/api",
            "/collections",
            "/collections/{collectionId}",
            "/collections/{collectionId}/items",
            "/collections/{collectionId}/items/{featureId}",
            "/conformance",
        ]
    );
}

/// The server is this endpoint's own OGC root, so a client that reads the document and follows a
/// path lands on this endpoint and not on another.
#[test]
fn the_only_server_is_this_endpoints_own_root() {
    let document = document(&["Depot"]);
    assert_eq!(
        document["servers"],
        json!([{ "url": format!("{ENDPOINT}/ogc/features"), "description": "This endpoint" }])
    );
}

/// Every operation carries an id of its own, because a client generator turns them into function
/// names and two of a kind collide.
#[test]
fn every_operation_id_is_used_once() {
    let document = document(&["Depot"]);
    let mut ids: Vec<&str> = paths(&document)
        .values()
        .filter_map(|item| item.get("get"))
        .filter_map(|get| get["operationId"].as_str())
        .collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), count, "{ids:?}");
    assert_eq!(count, 7, "one per path");
}

/// The items operation names every parameter the handler honours, and each of them is a query
/// parameter that is not required: a client must be able to ask for a page with none of them.
#[test]
fn the_items_operation_names_every_parameter_the_handler_honours() {
    let document = document(&["Depot"]);
    let parameters = paths(&document)["/collections/{collectionId}/items"]["get"]["parameters"]
        .as_array()
        .expect("parameters");

    let names: Vec<&str> = parameters
        .iter()
        .filter_map(|parameter| parameter["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "bbox",
            "datetime",
            "filter",
            "filter-lang",
            "crs",
            "limit",
            "next"
        ]
    );
    for parameter in parameters {
        assert_eq!(parameter["in"], json!("query"));
        assert_eq!(parameter["required"], json!(false), "{}", parameter["name"]);
    }
}

/// The filter language and the CRS are offered as the single values this endpoint accepts, so a
/// client that reads the document cannot ask for one the gateway would refuse.
#[test]
fn the_document_offers_only_the_filter_language_and_crs_that_are_accepted() {
    let document = document(&["Depot"]);
    let parameters = &paths(&document)["/collections/{collectionId}/items"]["get"]["parameters"];

    assert_eq!(parameters[3]["schema"]["enum"], json!(["cql2-text"]));
    assert_eq!(parameters[3]["schema"]["default"], json!("cql2-text"));
    assert_eq!(
        parameters[4]["schema"]["enum"],
        json!(["http://www.opengis.net/def/crs/OGC/1.3/CRS84"])
    );
}

/// A path parameter is required and a query parameter is not; a client generator reads exactly
/// this to decide what it must pass.
#[test]
fn a_path_parameter_is_required_and_carries_the_style_openapi_asks_for() {
    let document = document(&["Depot"]);
    let feature = &paths(&document)["/collections/{collectionId}/items/{featureId}"]["parameters"];

    for parameter in feature.as_array().expect("two path parameters") {
        assert_eq!(parameter["in"], json!("path"));
        assert_eq!(parameter["required"], json!(true));
        assert_eq!(parameter["style"], json!("simple"));
        assert_eq!(parameter["explode"], json!(false));
    }
}

/// The title and the description are the endpoint's own, carried as data: a title with a quote,
/// a tag or a newline is a JSON string and never structure.
#[test]
fn the_title_and_description_are_carried_as_data() {
    for text in ["Ovzdušie", "\"quoted\"", "</script>", "a\nnewline"] {
        let document = api_document(ENDPOINT, text, text, &["Depot".to_owned()]);
        assert_eq!(document["info"]["title"], json!(text));
        assert_eq!(document["info"]["description"], json!(text));
        let rendered = serde_json::to_string(&document).expect("serialises");
        assert!(!rendered.contains('\n'), "{text:?} broke the document");
    }
}

/// Every operation says what a client gets when it is refused, because a client that reads only
/// the `200` retries a `400` for ever.
#[test]
fn every_operation_documents_the_refusals_a_client_can_get() {
    let document = document(&["Depot"]);
    for (path, item) in paths(&document) {
        let Some(get) = item.get("get") else { continue };
        for status in ["200", "400", "404"] {
            assert!(
                get["responses"].get(status).is_some(),
                "{path} does not say what a {status} looks like"
            );
        }
    }
    assert!(document["components"]["responses"]["BadRequest"].is_object());
    assert!(document["components"]["responses"]["NotFound"].is_object());
}

/// The same caller gets the same document twice: nothing in it is a clock, a counter or a random
/// name a client would have to follow twice.
#[test]
fn the_same_input_renders_the_same_document() {
    assert_eq!(
        document(&["Depot", "Vehicle"]),
        document(&["Depot", "Vehicle"])
    );
}
