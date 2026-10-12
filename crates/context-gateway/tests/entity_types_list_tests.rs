//! `GET /ngsi-ld/v1/types` through an Endpoint (CIM 009 5.7.1, T-4080).
//!
//! On dev every public Endpoint answered it 404 or 403: the grants name `retrieveOps`, which does
//! not hold the type list, so a client had to read CKAN or the schema files to learn what to ask
//! for. A read grant now shows the types the Endpoint's reads show: the stub here is the view,
//! answering entities of the types it lets the caller read and nothing of the others.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use context_gateway::app::{router, Gateway};
use context_gateway::pdp::PolicyPdp;
use context_gateway::proxy::Broker;
use context_gateway::resolver::Endpoint;
use jc_core::kinds::{Audience, Representation};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const SLUG: &str = "mluyob4nz52lok3ssk7pgn5vwt";
const HELD: [&str; 5] = ["Alert", "GtfsStop", "Secret", "TransitRoute", "Vehicle"];

/// Every path the view was asked, with its query.
type Asked = Arc<Mutex<Vec<String>>>;

/// The space holds the five types; the view answers an entity of each type in `readable` and
/// nothing of the others, as the engine narrows a caller's read.
async fn view_broker(readable: &'static [&'static str]) -> (String, Asked) {
    let asked: Asked = Arc::default();
    let recorder = Arc::clone(&asked);
    let app = Router::new().fallback(move |request: Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            let path = request.uri().path().to_owned();
            let query = request.uri().query().unwrap_or_default().to_owned();
            recorder
                .lock()
                .expect("no poisoned lock")
                .push(format!("{path}?{query}"));
            if path == "/ngsi-ld/v1/types" {
                return axum::Json(json!({ "type": "EntityTypeList", "typeList": HELD }))
                    .into_response();
            }
            if path == format!("/x/views/{SLUG}/ngsi-ld/v1/types") {
                return (
                    StatusCode::FORBIDDEN,
                    axum::Json(json!({ "title": "AccessDenied", "status": 403 })),
                )
                    .into_response();
            }
            let asked_type = query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(name, _)| *name == "type")
                .map(|(_, value)| value.to_owned())
                .unwrap_or_default();
            let rows: Vec<Value> = readable
                .iter()
                .filter(|name| **name == asked_type)
                .map(|name| json!({ "id": format!("urn:ngsi-ld:{name}:1"), "type": name }))
                .collect();
            axum::Json(Value::Array(rows)).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a free port");
    let port = listener.local_addr().expect("an address").port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://127.0.0.1:{port}"), asked)
}

fn endpoint(operations: &str, granted: &[&str]) -> Endpoint {
    let information = if granted.is_empty() {
        String::new()
    } else {
        let entities: Vec<String> = granted
            .iter()
            .map(|name| format!("{{ type: {name} }}"))
            .collect();
        format!("information:\n  - entities: [{}]\n", entities.join(", "))
    };
    Endpoint {
        declared_types: None,
        roles: Default::default(),
        slug: SLUG.to_owned(),
        title: std::collections::BTreeMap::new(),
        description: std::collections::BTreeMap::new(),
        space: "air".to_owned(),
        project: "air".to_owned(),
        audience: Audience::Public,
        allowed_projects: Vec::new(),
        representations: vec![Representation::NgsiLd],
        rate_limit: None,
        creates: None,
        file_limits: None,
        hidden_attributes: BTreeSet::new(),
        projection: None,
        base_path: format!("/api/endpoint/{SLUG}"),
        models: Vec::new(),
        view_mapping: None,
        catalog: None,
        policy_names: Vec::new(),
        policies: vec![serde_norway::from_str(&format!(
            "contextSpaceRef: air\n\
             assigner: did:web:example.org\n\
             assignee: {{ kind: role, id: public }}\n\
             operations: {operations}\n\
             {information}"
        ))
        .expect("the policy spec parses")],
    }
}

/// An anonymous `GET /types`, as a client with no account sends it.
async fn types(broker: &str, endpoint: Endpoint) -> (StatusCode, Value) {
    let app = router(Arc::new(
        Gateway::new(Broker::new(broker), Box::new(PolicyPdp), "example.org").serve([endpoint]),
    ));
    let response = app
        .oneshot(
            HttpRequest::builder()
                .uri(format!("/api/endpoint/{SLUG}/ngsi-ld/v1/types"))
                .body(Body::empty())
                .expect("a request"),
        )
        .await
        .expect("the gateway answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a readable body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn listed(answer: &Value) -> Vec<&str> {
    answer["typeList"]
        .as_array()
        .expect("a typeList")
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

/// A public Endpoint whose Policy grants 2 of the 5 types lists those 2, to anyone.
#[tokio::test]
async fn a_read_grant_of_two_types_lists_those_two_to_anyone() {
    let (broker, asked) = view_broker(&["GtfsStop", "Vehicle"]).await;
    let (status, answer) =
        types(&broker, endpoint("[retrieveOps]", &["GtfsStop", "Vehicle"])).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["type"], json!("EntityTypeList"));
    assert_eq!(listed(&answer), ["GtfsStop", "Vehicle"]);
    // Only the granted types were asked of the view, and the view's own type list never.
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert!(
        asked.iter().all(|hop| !hop.contains("Secret")
            && !hop.starts_with(&format!("/x/views/{SLUG}/ngsi-ld/v1/types"))),
        "{asked:?}"
    );
}

/// A type the grant reaches but the view shows the caller nothing of is not listed: its name
/// would tell what the space holds.
#[tokio::test]
async fn a_type_the_view_shows_nothing_of_is_not_listed() {
    let (broker, _) = view_broker(&["Alert"]).await;
    let (status, answer) = types(&broker, endpoint("[retrieveOps]", &[])).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(listed(&answer), ["Alert"]);
}

/// A caller holding no read through the Endpoint is answered by the view, as before.
#[tokio::test]
async fn without_a_read_grant_the_view_answers() {
    let (broker, asked) = view_broker(&HELD).await;
    let (status, _) = types(&broker, endpoint("[createEntity]", &[])).await;
    assert_ne!(status, StatusCode::OK);
    let asked = asked.lock().expect("no poisoned lock").clone();
    assert!(
        asked.iter().all(|hop| !hop.contains("entities")),
        "no read was made on the caller's behalf: {asked:?}"
    );
}
