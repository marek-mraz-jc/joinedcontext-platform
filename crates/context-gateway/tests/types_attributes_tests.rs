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

mod common;

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
        let _ = axum::serve(listener, common::plain_tree(app)).await;
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
        declared_types: None,
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
        creates: None,
        file_limits: None,
        hidden_attributes: ["secretPin".to_owned()].into_iter().collect(),
        projection: Some(projection()),
        view_mapping: None,
        catalog: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Vehicle".to_owned(), "Depot".to_owned()],
            json_schema: None,
            context: None,
        }],
        policy_names: Vec::new(),
        policies: vec![policy()],
    }
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
