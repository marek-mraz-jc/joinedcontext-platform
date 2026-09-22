//! The discovery surface is narrowed like the data it describes (T-2134; EP-25, EP-26, MP-02).
//!
//! `GET /types`, `GET /types/{type}`, `GET /attributes` and `GET /attributes/{attr}` answer
//! documents *about* the space. They carry an `id` and a `type` like an entity does, which is
//! why the entity projection used to be applied to them: the allowed answer came back as
//! `{"id","type"}` with `attributeDetails` gone, and a name no grant reaches was forwarded and
//! answered `200` where a name that does not exist answers `404`. Two `200`s and one `404` is a
//! directory of the hidden names in the space, read one request at a time.
//!
//! The endpoint under test grants `Vehicle`, projects it to `name` and `location`, and hides
//! `secretPin`. So `odometer` is a real attribute outside the projection, `secretPin` a real
//! attribute the endpoint hides, `Depot` a real type no grant names, and `Ghost`/`ghostAttr`
//! names the broker itself does not know: the four cases whose answers may not be told apart.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::routing::any;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::{Endpoint, Model};
use jc_core::envelope::ResourceEnvelope;
use jc_core::kinds::{Audience, ModelProjectionSpec, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "kq2w9tzr4m7xhb3vsn6cdy8pgf";
const SPACE: &str = "fleet";
const DOMAIN: &str = "hel.fi";

const PARTNER_VIEW: &str = r#"apiVersion: joinedcontext.com/v1alpha1
kind: ModelProjection
metadata:
  name: partner-view
  namespace: helsinki
spec:
  contextSpaceRef: fleet
  dataModelRef: { kind: DataModel, name: fleet, version: "1" }
  classes:
    - name: Vehicle
      slots: [name, location]
"#;

fn projection() -> Arc<ModelProjectionSpec> {
    let parsed = ResourceEnvelope::<ModelProjectionSpec>::from_yaml(PARTNER_VIEW).expect("parses");
    parsed.validate().expect("valid");
    Arc::new(parsed.spec)
}

/// One `EntityType` entry as CIM 009 clause 5.2.25 shapes it.
fn entity_type(name: &str, attributes: &[&str]) -> Value {
    json!({
        "id": format!("https://uri.etsi.org/ngsi-ld/default-context/{name}"),
        "type": "EntityType",
        "typeName": name,
        "attributeNames": attributes,
    })
}

/// The vocabulary of a space that holds more than this endpoint serves.
fn vocabulary(path: &str) -> Option<Value> {
    let vehicle_attributes = ["location", "name", "odometer", "secretPin"];
    Some(match path {
        "/ngsi-ld/v1/types" => json!({
            "id": "urn:ngsi-ld:EntityTypeList:stub",
            "type": "EntityTypeList",
            "typeList": ["Depot", "Vehicle"],
        }),
        // The same request with `details=true`, which answers an array (clause 5.7.10).
        "/ngsi-ld/v1/types-details" => json!([
            entity_type("Depot", &["capacity", "name"]),
            entity_type("Vehicle", &vehicle_attributes),
        ]),
        "/ngsi-ld/v1/types/Vehicle" => json!({
            "id": "https://uri.etsi.org/ngsi-ld/default-context/Vehicle",
            "type": "EntityTypeInfo",
            "typeName": "Vehicle",
            "entityCount": 12,
            "attributeDetails": vehicle_attributes
                .iter()
                .map(|name| json!({
                    "id": format!("https://uri.etsi.org/ngsi-ld/default-context/{name}"),
                    "type": "Attribute",
                    "attributeName": name,
                    "attributeTypes": ["Property"],
                }))
                .collect::<Vec<Value>>(),
        }),
        "/ngsi-ld/v1/types/Depot" => json!({
            "id": "https://uri.etsi.org/ngsi-ld/default-context/Depot",
            "type": "EntityTypeInfo",
            "typeName": "Depot",
            "entityCount": 3,
            "attributeDetails": [{
                "id": "https://uri.etsi.org/ngsi-ld/default-context/capacity",
                "type": "Attribute",
                "attributeName": "capacity",
                "attributeTypes": ["Property"],
            }],
        }),
        "/ngsi-ld/v1/attributes" => json!({
            "id": "urn:ngsi-ld:AttributeList:stub",
            "type": "AttributeList",
            "attributeList": ["capacity", "location", "name", "odometer", "secretPin"],
        }),
        // The same request with `details=true`, an array of `Attribute` (clause 5.7.12).
        "/ngsi-ld/v1/attributes-details" => {
            json!(["capacity", "location", "name", "odometer", "secretPin"]
                .iter()
                .map(|name| json!({
                    "id": format!("https://uri.etsi.org/ngsi-ld/default-context/{name}"),
                    "type": "Attribute",
                    "attributeName": name,
                    "typeNames": ["Depot", "Vehicle"],
                    "attributeTypes": ["Property"],
                }))
                .collect::<Vec<Value>>())
        }
        "/ngsi-ld/v1/attributes/name"
        | "/ngsi-ld/v1/attributes/odometer"
        | "/ngsi-ld/v1/attributes/secretPin" => {
            let name = path.rsplit('/').next().unwrap_or_default();
            json!({
                "id": format!("https://uri.etsi.org/ngsi-ld/default-context/{name}"),
                "type": "Attribute",
                "attributeName": name,
                "typeNames": ["Depot", "Vehicle"],
                "attributeCount": 15,
                "attributeTypes": ["Property"],
            })
        }
        // Everything else is a name this space does not hold, and a broker says so with a 404.
        _ => return None,
    })
}

type Hops = Arc<Mutex<Vec<String>>>;

/// A broker that answers the vocabulary of the whole space, and `404` for a name it does not
/// hold: the answer the gateway's own refusal has to be indistinguishable from.
async fn broker() -> (String, Hops) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let mut path = request.uri().path().to_owned();
            if request
                .uri()
                .query()
                .unwrap_or_default()
                .contains("details=true")
            {
                path.push_str("-details");
            }
            recorder.lock().expect("the hop log").push(path.clone());
            match vocabulary(&path) {
                Some(document) => (StatusCode::OK, axum::Json(document)),
                None => (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({
                        "type": "https://uri.etsi.org/ngsi-ld/errors/ResourceNotFound",
                        "title": "ResourceNotFound",
                        "status": 404,
                        "detail": format!("{path} is not a name this space holds"),
                    })),
                ),
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), hops)
}

fn policy() -> PolicySpec {
    serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, retrieveEntityTypes, \
         retrieveEntityTypeDetails, retrieveEntityTypeInfo, retrieveAttrTypes, \
         retrieveAttrTypeDetails, retrieveAttrTypeInfo]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Vehicle\n"
    ))
    .expect("the policy spec parses")
}

fn endpoint() -> Endpoint {
    Endpoint {
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: SPACE.to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd, Representation::Mcp],
        rate_limit: None,
        file_limits: None,
        hidden_attributes: ["secretPin".to_owned()].into_iter().collect(),
        projection: Some(projection()),
        view_mapping: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned(), "Depot".to_owned()],
            json_schema: None,
            context: None,
        }],
        policies: vec![policy()],
    }
}

/// The same space read by an endpoint whose grant names no type and whose only narrowing is the
/// hidden attribute: the shape the leak was measured on, where every vocabulary document came
/// back as `200` because the type guard had no type to judge it against.
fn whole_space_endpoint() -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntity, retrieveEntityTypes, retrieveAttrTypes, \
         retrieveEntityTypeInfo, retrieveAttrTypeInfo]\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        roles: Default::default(),
        projection: None,
        policies: vec![policy],
        ..endpoint()
    }
}

/// An endpoint whose grant names a type this space does not hold: every listing narrows to
/// nothing, which is an empty listing and not an error.
fn stranger_endpoint() -> Endpoint {
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: {SPACE}\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [queryEntity, retrieveEntityTypes, retrieveAttrTypes]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Ghost\n"
    ))
    .expect("the policy spec parses");
    Endpoint {
        roles: Default::default(),
        projection: None,
        hidden_attributes: Default::default(),
        policies: vec![policy],
        ..endpoint()
    }
}

/// One request through the gateway, with the answer, its text and every path the broker was
/// asked for.
async fn ask(uri: &str) -> (StatusCode, Value, String, Vec<String>) {
    ask_through(endpoint(), uri).await
}

async fn ask_through(endpoint: Endpoint, uri: &str) -> (StatusCode, Value, String, Vec<String>) {
    let (upstream, hops) = broker().await;
    ask_upstream(endpoint, uri, upstream, hops).await
}

/// The same against a broker answering one document to every path, whatever the document is: a
/// gateway that trusts the shape of what comes back is a gateway a broker can crash.
async fn ask_answering(
    endpoint: Endpoint,
    uri: &str,
    document: Value,
) -> (StatusCode, Value, String, Vec<String>) {
    let hops: Hops = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&hops);
    let answer = Arc::new(document);
    let app = Router::new().fallback(any(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        let answer = Arc::clone(&answer);
        async move {
            recorder
                .lock()
                .expect("the hop log")
                .push(request.uri().path().to_owned());
            axum::Json((*answer).clone())
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let address = listener.local_addr().expect("the bound address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    ask_upstream(endpoint, uri, format!("http://{address}"), hops).await
}

async fn ask_upstream(
    endpoint: Endpoint,
    uri: &str,
    upstream: String,
    hops: Hops,
) -> (StatusCode, Value, String, Vec<String>) {
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint]),
    );
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/endpoint/{SLUG}{uri}"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let asked = hops.lock().expect("the hop log").clone();
    (status, body, raw, asked)
}

/// One MCP tool call, with the whole JSON-RPC answer as text.
async fn tool(name: &str) -> String {
    let (upstream, _) = broker().await;
    let gateway = Arc::new(
        Gateway::new(Broker::new(upstream), Box::new(PolicyPdp), DOMAIN).serve([endpoint()]),
    );
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": { "details": true } }
    });
    let response = router(gateway)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/endpoint/{SLUG}/mcp"))
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from(call.to_string()))
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("a body");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// EP-25: an attribute the endpoint hides is not found, and the broker is never asked, so
/// neither the status nor the timing says the name exists.
#[tokio::test]
async fn a_hidden_attribute_is_not_found_and_the_broker_is_not_asked() {
    let (status, _, raw, asked) = ask("/ngsi-ld/v1/attributes/secretPin").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{raw}");
    assert!(
        asked.is_empty(),
        "the broker was asked about a hidden name: {asked:?}"
    );
    assert!(
        !raw.contains("secretPin"),
        "the refusal repeats the hidden name: {raw}"
    );
}

/// EP-26, MP-02: a type or attribute outside the projection answers exactly what a name the
/// space does not hold answers. The four bodies are compared, not only the statuses: a refusal
/// that words itself differently is the same oracle in prose.
#[tokio::test]
async fn a_name_outside_the_projection_is_not_found_like_a_name_that_does_not_exist() {
    let unknown = ask("/ngsi-ld/v1/types/Ghost").await;
    assert_eq!(unknown.0, StatusCode::NOT_FOUND, "{}", unknown.2);

    for withheld in [
        "/ngsi-ld/v1/types/Depot",
        "/ngsi-ld/v1/attributes/odometer",
        "/ngsi-ld/v1/attributes/ghostAttr",
    ] {
        let (status, body, raw, _) = ask(withheld).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{withheld}: {raw}");
        assert_eq!(
            body, unknown.1,
            "{withheld} answers a body of its own, which tells the caller the name exists"
        );
    }
}

/// EP-25, as measured on 2026-09-18: with no type grant to judge the document against, every
/// vocabulary document came back `200`, so asking for one hidden name after another told the
/// caller which of them the space holds. The endpoint hides `secretPin` and serves the rest.
#[tokio::test]
async fn a_hidden_name_is_not_found_even_when_the_grant_names_no_type() {
    let hidden = ask_through(whole_space_endpoint(), "/ngsi-ld/v1/attributes/secretPin").await;
    let unknown = ask_through(whole_space_endpoint(), "/ngsi-ld/v1/attributes/ghostAttr").await;
    assert_eq!(hidden.0, StatusCode::NOT_FOUND, "{}", hidden.2);
    assert_eq!(
        hidden.1, unknown.1,
        "the hidden name answers something of its own: {}",
        hidden.2
    );

    // And the rest of the space is still described, counts apart: a guard that answers 404 for
    // everything would pass the assertion above and serve nobody.
    let (status, body, raw, _) = ask_through(whole_space_endpoint(), "/ngsi-ld/v1/types").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(body["typeList"], json!(["Depot", "Vehicle"]), "{raw}");
    let (status, body, raw, _) =
        ask_through(whole_space_endpoint(), "/ngsi-ld/v1/types/Depot").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(body["typeName"], json!("Depot"), "{raw}");
    let (_, body, raw, _) = ask_through(whole_space_endpoint(), "/ngsi-ld/v1/attributes").await;
    assert!(
        !raw.contains("secretPin"),
        "the attribute list names the hidden attribute: {raw}"
    );
    assert_eq!(
        body["attributeList"],
        json!(["capacity", "location", "name", "odometer"]),
        "{raw}"
    );
}

/// MP-02: the allowed answer is the broker's document, narrowed — not an entity stripped to its
/// identity. `attributeDetails` is what a client reads to know the shape of the type.
#[tokio::test]
async fn an_allowed_type_keeps_its_attribute_details() {
    let (status, body, raw, _) = ask("/ngsi-ld/v1/types/Vehicle").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(body["typeName"], json!("Vehicle"), "{raw}");
    let details = body["attributeDetails"]
        .as_array()
        .unwrap_or_else(|| panic!("the type info keeps its attribute details: {raw}"));
    let named: Vec<&str> = details
        .iter()
        .filter_map(|entry| entry["attributeName"].as_str())
        .collect();
    assert_eq!(named, vec!["location", "name"], "{raw}");
    for withheld in ["odometer", "secretPin"] {
        assert!(
            !raw.contains(withheld),
            "the type info names an attribute this endpoint does not serve ({withheld}): {raw}"
        );
    }
}

/// EP-26: the listings name what the caller may read and nothing else, whichever of the four
/// listing operations asks.
#[tokio::test]
async fn the_listings_name_only_what_the_caller_may_read() {
    let (status, body, raw, _) = ask("/ngsi-ld/v1/types").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(body["typeList"], json!(["Vehicle"]), "{raw}");

    let (status, body, raw, _) = ask("/ngsi-ld/v1/types?details=true").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    let listed: Vec<&str> = body
        .as_array()
        .unwrap_or_else(|| panic!("the details listing is an array: {raw}"))
        .iter()
        .filter_map(|entry| entry["typeName"].as_str())
        .collect();
    assert_eq!(listed, vec!["Vehicle"], "{raw}");
    assert!(
        raw.contains("location") && !raw.contains("odometer"),
        "the listed type keeps its projected attributes and no others: {raw}"
    );

    let (status, body, raw, _) = ask("/ngsi-ld/v1/attributes").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(
        body["attributeList"],
        json!(["location", "name"]),
        "an attribute list is a list of what this endpoint serves: {raw}"
    );
}

/// EP-26: `typeNames` is a list of types, and it is narrowed like the type list itself.
#[tokio::test]
async fn type_names_of_an_attribute_list_only_granted_types() {
    let (status, body, raw, _) = ask("/ngsi-ld/v1/attributes/name").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(body["attributeName"], json!("name"), "{raw}");
    assert_eq!(body["typeNames"], json!(["Vehicle"]), "{raw}");
    assert!(
        !raw.contains("Depot"),
        "the attribute says which types have it, including one no grant names: {raw}"
    );
}

/// EP-25: a count is an aggregate over entities, and a narrowed caller may not have been
/// allowed to see all of them — `entityCount: 12` over one visible Vehicle is a number about
/// the eleven the grants withhold.
#[tokio::test]
async fn counts_do_not_include_what_the_caller_cannot_see() {
    let (_, body, raw, _) = ask("/ngsi-ld/v1/types/Vehicle").await;
    assert!(
        body.get("entityCount").is_none(),
        "the narrowed type info carries a count over entities the caller cannot see: {raw}"
    );
    let (_, body, raw, _) = ask("/ngsi-ld/v1/attributes/name").await;
    assert!(
        body.get("attributeCount").is_none(),
        "the narrowed attribute info carries a count of instances the caller cannot see: {raw}"
    );
}

/// AG-29: the two MCP listing tools are the same operations over the same handler, so they are
/// narrowed the same way. A model reading the space through them sees the endpoint's vocabulary.
#[tokio::test]
async fn the_mcp_listings_are_narrowed_like_the_ngsi_ld_ones() {
    for name in ["list_types", "list_attributes"] {
        let answer = tool(name).await;
        assert!(
            !answer.contains("Depot")
                && !answer.contains("odometer")
                && !answer.contains("secretPin"),
            "{name} named what this endpoint does not serve: {answer}"
        );
        assert!(
            answer.contains("Vehicle") || answer.contains("location"),
            "{name} answered nothing at all: {answer}"
        );
    }
}

/// EP-26: a caller may address a type by the expanded IRI the broker stores, and the grant names
/// the term. The two are the same name, so the request is forwarded rather than refused — the
/// mistake the other way round would let `https://elsewhere.example/Depot` past a `Depot` grant.
#[tokio::test]
async fn an_expanded_iri_is_the_same_name_as_the_term_the_grant_uses() {
    let (_, _, raw, asked) =
        ask("/ngsi-ld/v1/types/https%3A%2F%2Furi.etsi.org%2Fngsi-ld%2Fdefault-context%2FVehicle")
            .await;
    assert!(
        !asked.is_empty(),
        "the expanded form of a granted type was refused as an unknown name: {raw}"
    );

    let (status, _, raw, asked) =
        ask("/ngsi-ld/v1/types/https%3A%2F%2Felsewhere.example%2FDepot").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an expanded type no grant names is served: {raw}"
    );
    assert!(
        asked.is_empty(),
        "the broker was asked about a type no grant names: {asked:?}"
    );
}

/// EP-26: a grant over a type the space does not hold leaves the listings empty, and an empty
/// listing is an answer. Answering `404` instead would say the endpoint has no discovery surface,
/// and answering the broker's list would say what the space holds.
#[tokio::test]
async fn a_listing_narrowed_to_nothing_is_an_empty_listing() {
    let (status, body, raw, _) = ask_through(stranger_endpoint(), "/ngsi-ld/v1/types").await;
    assert_eq!(status, StatusCode::OK, "{raw}");
    assert_eq!(body["typeList"], json!([]), "{raw}");
    assert!(
        !raw.contains("Depot") && !raw.contains("Vehicle"),
        "the emptied listing still names the types of the space: {raw}"
    );
}

/// A broker is not trusted for the shape of its own documents either: a list of numbers, a null
/// list, a missing member and a bare array all have to come back as something, never a panic.
#[tokio::test]
async fn a_vocabulary_document_of_the_wrong_shape_is_answered_not_trusted() {
    for document in [
        json!({ "id": "urn:ngsi-ld:EntityTypeList:stub", "type": "EntityTypeList",
                "typeList": [7, null, { "Vehicle": true }, "Vehicle"] }),
        json!({ "id": "urn:ngsi-ld:EntityTypeList:stub", "type": "EntityTypeList",
                "typeList": Value::Null }),
        json!({ "id": "urn:ngsi-ld:EntityTypeList:stub", "type": "EntityTypeList" }),
        json!([]),
        json!("not a document at all"),
    ] {
        let (status, body, raw, _) =
            ask_answering(endpoint(), "/ngsi-ld/v1/types", document.clone()).await;
        assert!(
            status.is_success(),
            "the gateway could not answer {document}: {status} {raw}"
        );
        // Whatever the broker put in the list, what leaves is a list of names this grant reaches.
        if let Some(listed) = body.get("typeList").and_then(Value::as_array) {
            assert_eq!(
                listed,
                &vec![json!("Vehicle")],
                "the narrowed list kept something that is not a granted type name: {raw}"
            );
        }
    }
}

/// R20: the broker's own miss stays a miss. A name the grants reach that the space does not hold
/// is `404`, with the gateway's wording rather than the broker's, which is the same body the
/// refusal of a withheld name carries.
#[tokio::test]
async fn a_granted_name_the_space_does_not_hold_is_the_same_not_found() {
    // `Vehicle` is granted and projected; the stub holds no `/attributes/location` document.
    let (status, body, raw, asked) = ask("/ngsi-ld/v1/attributes/location").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{raw}");
    assert!(
        !asked.is_empty(),
        "a name the grants reach is looked up rather than refused: {asked:?}"
    );
    let (_, withheld, _, _) = ask("/ngsi-ld/v1/attributes/odometer").await;
    assert_eq!(
        body, withheld,
        "the broker's miss and the gateway's refusal answer different bodies: {raw}"
    );
}

/// EP-26, T-2131: the broker is not the authority on which name it answered about. A document
/// that comes back about an attribute the caller may not reach is not served, however the path
/// that asked for it was spelled — an alias, a registration answering for a neighbour, a defect.
#[tokio::test]
async fn a_document_about_another_name_than_the_path_asked_for_is_not_served() {
    let answered_about_something_else = json!({
        "id": "https://uri.etsi.org/ngsi-ld/default-context/odometer",
        "type": "Attribute",
        "attributeName": "odometer",
        "typeNames": ["Vehicle"],
    });
    let (status, _, raw, _) = ask_answering(
        endpoint(),
        "/ngsi-ld/v1/types/Vehicle",
        answered_about_something_else,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{raw}");
    assert!(
        !raw.contains("odometer"),
        "the broker chose which attribute this endpoint serves: {raw}"
    );
}
