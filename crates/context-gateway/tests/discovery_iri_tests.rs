//! Discovery compares names as IRIs, the way entity reads do since T-3473 (T-3533; EP-25, EP-26).
//!
//! `https://a.example/Car` and `https://b.example/Car` share the term `Car` and are two types.
//! A grant on the first, written `Car` and defined by the space's model, used to reveal the
//! second through `/types`, `/types/{type}`, `/attributes` and `/attributes/{attr}`, because the
//! vocabulary guard cut every name to its last path segment before comparing. The broker here
//! holds both vocabularies; every answer the gateway serves may name only the granted one.

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
use jc_core::kinds::{Audience, PolicySpec, Representation};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "r5t8w2qz9k4mxb7vhn3cdy6pgf";
const DOMAIN: &str = "hel.fi";
const A_CAR: &str = "https://a.example/Car";
const B_CAR: &str = "https://b.example/Car";
const A_PLATE: &str = "https://a.example/plate";
const B_PLATE: &str = "https://b.example/plate";
const A_PIN: &str = "https://a.example/pin";

fn attribute(name: &str) -> Value {
    json!({
        "id": name, "type": "Attribute", "attributeName": name,
        "typeNames": [A_CAR, B_CAR], "attributeCount": 4, "attributeTypes": ["Property"],
    })
}

fn type_info(name: &str) -> Value {
    let details: Vec<Value> = [A_PLATE, B_PLATE, A_PIN]
        .map(|attribute| {
            json!({
                "id": attribute, "type": "Attribute", "attributeName": attribute,
                "attributeTypes": ["Property"],
            })
        })
        .into();
    json!({
        "id": name, "type": "EntityTypeInfo", "typeName": name, "entityCount": 2,
        "attributeDetails": details,
    })
}

/// A broker holding both vocabularies, answering the IRIs as the core context leaves them.
fn vocabulary(path: &str) -> Option<Value> {
    let path = context_gateway::query::decode(path);
    let name = path.split_once("/ngsi-ld/v1/").map(|(_, rest)| rest)?;
    Some(match name {
        "types" => json!({
            "id": "urn:ngsi-ld:EntityTypeList:stub", "type": "EntityTypeList",
            "typeList": ["Car", A_CAR, B_CAR],
        }),
        "types-details" => Value::Array(
            [A_CAR, B_CAR]
                .map(|name| {
                    json!({
                        "id": name, "type": "EntityType", "typeName": name,
                        "attributeNames": [A_PLATE, B_PLATE, A_PIN],
                    })
                })
                .into(),
        ),
        "attributes" => json!({
            "id": "urn:ngsi-ld:AttributeList:stub", "type": "AttributeList",
            "attributeList": [A_PLATE, B_PLATE, A_PIN],
        }),
        "attributes-details" => Value::Array([A_PLATE, B_PLATE, A_PIN].map(attribute).into()),
        _ => match name.split_once('/') {
            Some(("types", name)) if [A_CAR, B_CAR, "Car"].contains(&name) => type_info(name),
            Some(("attributes", name)) if [A_PLATE, B_PLATE, A_PIN].contains(&name) => {
                attribute(name)
            }
            _ => return None,
        },
    })
}

type Hops = Arc<Mutex<Vec<String>>>;

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
                        "title": "ResourceNotFound", "status": 404,
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

/// A grant on `Car` with `plate` whitelisted when `attributes` is set, and `secret` hidden.
fn endpoint(context: Option<Value>, whitelist: bool) -> Endpoint {
    let attributes = if whitelist {
        "\x20   propertyNames: [plate]\n"
    } else {
        ""
    };
    let policy: PolicySpec = serde_norway::from_str(&format!(
        "contextSpaceRef: fleet\n\
         assigner: did:web:{DOMAIN}\n\
         assignee: {{ kind: role, id: public }}\n\
         operations: [retrieveEntityTypes, retrieveEntityTypeDetails, retrieveEntityTypeInfo, \
         retrieveAttrTypes, retrieveAttrTypeDetails, retrieveAttrTypeInfo, queryEntity]\n\
         information:\n\
         \x20 - entities:\n\
         \x20     - type: Car\n\
         {attributes}"
    ))
    .expect("the policy spec parses");
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: Default::default(),
        description: Default::default(),
        space: "fleet".to_owned(),
        project: "helsinki".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: ["secret".to_owned()].into_iter().collect(),
        projection: None,
        view_mapping: None,
        catalog: None,
        deprecation: None,
        feeds: Vec::new(),
        calendars: Vec::new(),
        map_defaults: Default::default(),
        base_path: format!("/api/endpoint/{SLUG}"),
        models: vec![Model {
            name: "fleet".to_owned(),
            version: "1.0.0".to_owned(),
            major: 1,
            classes: vec!["Car".to_owned()],
            json_schema: None,
            context: context.map(Into::into),
        }],
        policy_names: Vec::new(),
        manifest_name: String::new(),
        policies: vec![policy],
    }
}

/// The space's model: `Car`, `plate` and `secret` are the `a.example` vocabulary.
fn model_context() -> Option<Value> {
    Some(json!({ "@context": { "Car": A_CAR, "plate": A_PLATE, "secret": { "@id": A_PIN } } }))
}

async fn ask(endpoint: Endpoint, uri: &str) -> (StatusCode, Value, String, Vec<String>) {
    let (upstream, hops) = broker().await;
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

/// The EntityMap is not a surface of an endpoint: no route answers it, so it names nothing.
#[tokio::test]
async fn the_entity_map_is_not_served() {
    let (status, _, raw, asked) = ask(
        endpoint(model_context(), false),
        "/ngsi-ld/v1/entityMap/urn:ngsi-ld:EntityMap:x",
    )
    .await;
    assert!(status.is_client_error(), "{status}: {raw}");
    assert!(asked.is_empty(), "the broker was asked: {asked:?}");
}
